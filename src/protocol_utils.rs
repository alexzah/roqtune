//! Helper implementations for protocol patch types.

use crate::protocol::{
    BufferingConfigDelta, CastConfigDelta, IntegrationsConfigDelta, LibraryConfigDelta,
    OutputConfigDelta, UiConfigDelta,
};

impl OutputConfigDelta {
    pub fn is_empty(&self) -> bool {
        self.output_device_name.is_none()
            && self.output_device_auto.is_none()
            && self.channel_count.is_none()
            && self.sample_rate_khz.is_none()
            && self.bits_per_sample.is_none()
            && self.channel_count_auto.is_none()
            && self.sample_rate_auto.is_none()
            && self.bits_per_sample_auto.is_none()
            && self.resampler_quality.is_none()
            && self.dither_on_bitdepth_reduce.is_none()
            && self.downmix_higher_channel_tracks.is_none()
    }

    pub fn merge_from(&mut self, newer: Self) {
        if newer.output_device_name.is_some() {
            self.output_device_name = newer.output_device_name;
        }
        if newer.output_device_auto.is_some() {
            self.output_device_auto = newer.output_device_auto;
        }
        if newer.channel_count.is_some() {
            self.channel_count = newer.channel_count;
        }
        if newer.sample_rate_khz.is_some() {
            self.sample_rate_khz = newer.sample_rate_khz;
        }
        if newer.bits_per_sample.is_some() {
            self.bits_per_sample = newer.bits_per_sample;
        }
        if newer.channel_count_auto.is_some() {
            self.channel_count_auto = newer.channel_count_auto;
        }
        if newer.sample_rate_auto.is_some() {
            self.sample_rate_auto = newer.sample_rate_auto;
        }
        if newer.bits_per_sample_auto.is_some() {
            self.bits_per_sample_auto = newer.bits_per_sample_auto;
        }
        if newer.resampler_quality.is_some() {
            self.resampler_quality = newer.resampler_quality;
        }
        if newer.dither_on_bitdepth_reduce.is_some() {
            self.dither_on_bitdepth_reduce = newer.dither_on_bitdepth_reduce;
        }
        if newer.downmix_higher_channel_tracks.is_some() {
            self.downmix_higher_channel_tracks = newer.downmix_higher_channel_tracks;
        }
    }
}

impl CastConfigDelta {
    pub fn is_empty(&self) -> bool {
        self.allow_transcode_fallback.is_none()
    }
}

impl UiConfigDelta {
    pub fn is_empty(&self) -> bool {
        self.show_layout_edit_intro.is_none()
            && self.show_tooltips.is_none()
            && self.auto_scroll_to_playing_track.is_none()
            && self.dark_mode.is_none()
            && self.playlist_album_art_column_min_width_px.is_none()
            && self.playlist_album_art_column_max_width_px.is_none()
            && self.layout.is_none()
            && self.playlist_columns.is_none()
            && self.window_width.is_none()
            && self.window_height.is_none()
            && self.volume.is_none()
            && self.playback_order.is_none()
            && self.repeat_mode.is_none()
    }

    pub fn merge_from(&mut self, newer: Self) {
        if newer.show_layout_edit_intro.is_some() {
            self.show_layout_edit_intro = newer.show_layout_edit_intro;
        }
        if newer.show_tooltips.is_some() {
            self.show_tooltips = newer.show_tooltips;
        }
        if newer.auto_scroll_to_playing_track.is_some() {
            self.auto_scroll_to_playing_track = newer.auto_scroll_to_playing_track;
        }
        if newer.dark_mode.is_some() {
            self.dark_mode = newer.dark_mode;
        }
        if newer.playlist_album_art_column_min_width_px.is_some() {
            self.playlist_album_art_column_min_width_px =
                newer.playlist_album_art_column_min_width_px;
        }
        if newer.playlist_album_art_column_max_width_px.is_some() {
            self.playlist_album_art_column_max_width_px =
                newer.playlist_album_art_column_max_width_px;
        }
        if newer.layout.is_some() {
            self.layout = newer.layout;
        }
        if newer.playlist_columns.is_some() {
            self.playlist_columns = newer.playlist_columns;
        }
        if newer.window_width.is_some() {
            self.window_width = newer.window_width;
        }
        if newer.window_height.is_some() {
            self.window_height = newer.window_height;
        }
        if newer.volume.is_some() {
            self.volume = newer.volume;
        }
        if newer.playback_order.is_some() {
            self.playback_order = newer.playback_order;
        }
        if newer.repeat_mode.is_some() {
            self.repeat_mode = newer.repeat_mode;
        }
    }
}

impl LibraryConfigDelta {
    pub fn is_empty(&self) -> bool {
        self.folders.is_none()
            && self.online_metadata_enabled.is_none()
            && self.online_metadata_prompt_pending.is_none()
            && self.list_image_max_edge_px.is_none()
            && self.cover_art_cache_max_size_mb.is_none()
            && self.cover_art_memory_cache_max_size_mb.is_none()
            && self.artist_image_memory_cache_max_size_mb.is_none()
            && self.artist_image_cache_ttl_days.is_none()
            && self.artist_image_cache_max_size_mb.is_none()
    }

    pub fn merge_from(&mut self, newer: Self) {
        if newer.folders.is_some() {
            self.folders = newer.folders;
        }
        if newer.online_metadata_enabled.is_some() {
            self.online_metadata_enabled = newer.online_metadata_enabled;
        }
        if newer.online_metadata_prompt_pending.is_some() {
            self.online_metadata_prompt_pending = newer.online_metadata_prompt_pending;
        }
        if newer.list_image_max_edge_px.is_some() {
            self.list_image_max_edge_px = newer.list_image_max_edge_px;
        }
        if newer.cover_art_cache_max_size_mb.is_some() {
            self.cover_art_cache_max_size_mb = newer.cover_art_cache_max_size_mb;
        }
        if newer.cover_art_memory_cache_max_size_mb.is_some() {
            self.cover_art_memory_cache_max_size_mb = newer.cover_art_memory_cache_max_size_mb;
        }
        if newer.artist_image_memory_cache_max_size_mb.is_some() {
            self.artist_image_memory_cache_max_size_mb =
                newer.artist_image_memory_cache_max_size_mb;
        }
        if newer.artist_image_cache_ttl_days.is_some() {
            self.artist_image_cache_ttl_days = newer.artist_image_cache_ttl_days;
        }
        if newer.artist_image_cache_max_size_mb.is_some() {
            self.artist_image_cache_max_size_mb = newer.artist_image_cache_max_size_mb;
        }
    }
}

impl BufferingConfigDelta {
    pub fn is_empty(&self) -> bool {
        self.player_low_watermark_ms.is_none()
            && self.player_target_buffer_ms.is_none()
            && self.player_request_interval_ms.is_none()
            && self.decoder_request_chunk_ms.is_none()
    }

    pub fn merge_from(&mut self, newer: Self) {
        if newer.player_low_watermark_ms.is_some() {
            self.player_low_watermark_ms = newer.player_low_watermark_ms;
        }
        if newer.player_target_buffer_ms.is_some() {
            self.player_target_buffer_ms = newer.player_target_buffer_ms;
        }
        if newer.player_request_interval_ms.is_some() {
            self.player_request_interval_ms = newer.player_request_interval_ms;
        }
        if newer.decoder_request_chunk_ms.is_some() {
            self.decoder_request_chunk_ms = newer.decoder_request_chunk_ms;
        }
    }
}

impl IntegrationsConfigDelta {
    pub fn is_empty(&self) -> bool {
        self.backends.is_none()
    }
}
