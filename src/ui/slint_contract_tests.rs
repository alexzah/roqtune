#[cfg(test)]
mod tests {
    #[test]
    fn test_import_menu_exposes_add_files_and_add_folder_options() {
        let slint_ui = include_str!("../roqtune.slint");
        assert!(
            slint_ui.contains("in-out property <bool> show_import_menu: false;"),
            "App window should expose import menu state"
        );
        assert!(
            slint_ui.contains("text: \"Add files\"")
                && slint_ui.contains("text: \"Add folders\"")
                && slint_ui.contains("text: \"Create new playlist\""),
            "Import menu should expose Add files, Add folders, and Create new playlist actions"
        );
        assert!(
            slint_ui.contains("callback open_folder();"),
            "App window should expose folder-import callback"
        );
    }

    #[test]
    fn test_library_view_shows_add_folder_cta_when_no_folders_are_configured() {
        let slint_ui = include_str!("../roqtune.slint");
        assert!(
            slint_ui.contains(
                "property <bool> has-configured-folders: root.settings_library_folders.length > 0;"
            ),
            "Library list container should detect when no folders are configured"
        );
        assert!(
            slint_ui.contains("if !library-list-container.has-configured-folders : Rectangle {")
                && slint_ui.contains("text: \"Add folders to get started\"")
                && slint_ui.contains("root.library_add_folder();"),
            "Library view should expose an in-view call-to-action to add folders"
        );
    }

    #[test]
    fn test_library_folder_menu_exposes_add_and_configure_actions() {
        let slint_ui = include_str!("../roqtune.slint");
        assert!(
            slint_ui.contains("in-out property <bool> show_library_folder_menu: false;"),
            "App window should expose library folder menu state"
        );
        assert!(
            slint_ui.contains("text: \"Add folder\"")
                && slint_ui.contains("root.library_add_folder();"),
            "Library folder menu should expose a direct Add folder action"
        );
        assert!(
            slint_ui.contains("text: \"Configure folders ...\"")
                && slint_ui.contains("root.open_settings();")
                && slint_ui.contains("root.settings_dialog_tab_index = 2;"),
            "Library folder menu should open settings directly to the Library tab"
        );
    }

    #[test]
    fn test_settings_menu_exposes_layout_edit_toggle_and_settings_entry() {
        let slint_ui = include_str!("../roqtune.slint");
        assert!(
            slint_ui.contains("in-out property <bool> show_settings_menu: false;"),
            "App window should expose settings action menu state"
        );
        assert!(
            slint_ui.contains("if (action-id == 10) {")
                && slint_ui.contains("root.show_settings_menu = true;"),
            "Settings action should open the settings action menu instead of the full settings dialog directly"
        );
        assert!(
            slint_ui.contains("text: \"Layout Editing Mode\"")
                && slint_ui.contains("root.open_layout_editor();"),
            "Settings action menu should expose layout editing mode toggle"
        );
        assert!(
            slint_ui.contains("quick-layout-toggle := Switch {")
                && slint_ui.contains("checked: root.layout_edit_mode;")
                && slint_ui.contains("width: 36px;"),
            "Settings action menu layout mode control should use compact switch control"
        );
        assert!(
            slint_ui.contains("text: \"Settings\"") && slint_ui.contains("root.open_settings();"),
            "Settings action menu should still expose normal settings dialog entry"
        );
    }

    #[test]
    fn test_settings_dialog_exposes_layout_tutorial_visibility_toggle() {
        let slint_ui = include_str!("../roqtune.slint");
        assert!(
            slint_ui.contains("in-out property <bool> settings_show_layout_edit_tutorial: true;"),
            "Settings dialog should expose tutorial visibility state"
        );
        assert!(
            slint_ui.contains("in-out property <bool> settings_show_tooltips: true;")
                && slint_ui.contains("in-out property <bool> show_tooltips_enabled: true;"),
            "Settings dialog should expose tooltip settings and runtime state"
        );
        assert!(
            slint_ui
                .contains("in-out property <bool> settings_auto_scroll_to_playing_track: true;"),
            "Settings dialog should expose auto-scroll playing-track setting"
        );
        assert!(
            slint_ui.contains("in-out property <bool> settings_dark_mode: true;"),
            "Settings dialog should expose persisted dark-mode setting"
        );
        assert!(
            slint_ui.contains("in-out property <int> settings_dialog_tab_index: 0;")
                && slint_ui
                    .contains("labels: [\"General\", \"Audio\", \"Library\", \"Integrations\"];"),
            "Settings dialog should expose and render General/Audio/Library/Integrations tabs"
        );
        assert!(
            slint_ui.contains("text: \"Show layout editing mode tutorial\""),
            "Settings dialog should provide tutorial visibility toggle row"
        );
        assert!(
            slint_ui.contains("if root.settings_dialog_tab_index == 0 : ScrollView {")
                && slint_ui.contains("settings-layout-intro-toggle := Switch {")
                && slint_ui.contains("x: parent.width - self.width - 8px;")
                && slint_ui.contains("checked <=> root.settings_show_layout_edit_tutorial;"),
            "General tab tutorial row should keep compact right-aligned switch binding"
        );
        assert!(
            slint_ui.contains("if root.settings_dialog_tab_index == 2 : VerticalLayout {")
                && slint_ui.contains("text: \"Library Folders\"")
                && slint_ui.contains("text: \"Add Folder\"")
                && slint_ui.contains("text: \"Rescan\""),
            "Library tab should expose folder management controls"
        );
        assert!(
            slint_ui.contains("root.settings_show_layout_edit_tutorial"),
            "Settings apply flow should submit tutorial visibility flag"
        );
        assert!(
            slint_ui.contains("text: \"Show tooltips\"")
                && slint_ui.contains("checked <=> root.settings_show_tooltips;"),
            "General tab should expose tooltip visibility toggle row"
        );
        assert!(
            slint_ui.contains("text: \"Auto scroll to playing track\"")
                && slint_ui.contains("checked <=> root.settings_auto_scroll_to_playing_track;"),
            "General tab should expose auto-scroll playing-track toggle row"
        );
        assert!(
            slint_ui.contains("text: \"Appearance\"")
                && slint_ui.contains("text: \"Dark mode\"")
                && slint_ui.contains("checked <=> root.settings_dark_mode;"),
            "General tab should expose appearance section with dark-mode toggle"
        );
        assert!(
            slint_ui.contains(
                "callback apply_settings(int, int, int, int, string, string, string, string, bool, bool, bool, bool, int, int, bool, bool, bool);"
            ),
            "Apply settings callback should include auto-scroll, dark mode, sample-rate mode, resampler quality, dither, and downmix controls"
        );
        assert!(
            slint_ui.contains("label: \"Output Sample Rate\"")
                && slint_ui.contains("options: root.settings_sample_rate_mode_options;")
                && slint_ui.contains("selected_index <=> root.settings_sample_rate_mode_index;")
                && slint_ui.contains("if root.settings_dialog_tab_index == 1 : ScrollView {"),
            "Audio tab should expose Match Content and Manual sample-rate mode selector"
        );
    }

    #[test]
    fn test_focus_touch_areas_are_not_full_pane_overlays() {
        let slint_ui = include_str!("../roqtune.slint");

        assert!(
            !slint_ui.contains("sidebar-ta := TouchArea"),
            "Sidebar-wide focus TouchArea should not exist"
        );
        assert!(
            !slint_ui.contains("Rectangle {\n                horizontal-stretch: 4;\n                background: #282828;\n                \n                TouchArea {"),
            "Main-pane-wide focus TouchArea should not exist"
        );
        assert!(
            slint_ui.contains("root.sidebar_has_focus = false;"),
            "Track list interactions should clear sidebar focus"
        );
    }

    #[test]
    fn test_playlist_null_column_and_empty_space_trigger_deselect() {
        let slint_ui = include_str!("../roqtune.slint");

        assert!(
            slint_ui.contains("property <length> null-column-width: 120px;"),
            "App window should define a null column width"
        );
        assert!(
            slint_ui.contains("property <bool> in-null-column: self.in-viewport")
                && slint_ui.contains("track-list.visible-width - root.null-column-width"),
            "Track overlay should detect clicks in null column"
        );
        assert!(
            slint_ui.contains(
                "property <bool> is-in-rows: self.in-viewport && raw-row >= 0 && raw-row < root.track_model.length;"
            ),
            "Track overlay should detect when click is outside rows"
        );
        assert!(
            slint_ui.contains("root.deselect_all();"),
            "Track overlay should deselect on null-column or empty-space clicks"
        );
    }

    #[test]
    fn test_custom_column_menu_supports_delete_with_confirmation() {
        let slint_ui = include_str!("../roqtune.slint");
        let menu_ui = include_str!("components/menus.slint");

        assert!(
            menu_ui.contains("in property <[bool]> custom: [];"),
            "Column header menu should receive custom-column flags"
        );
        assert!(
            menu_ui.contains("callback delete-column(int);"),
            "Column header menu should expose delete callback"
        );
        assert!(
            menu_ui.contains("close-policy: PopupClosePolicy.close-on-click-outside;"),
            "Column header menu should only auto-close when clicking outside"
        );
        assert!(
            menu_ui.contains("column-toggle := Switch {")
                && menu_ui.contains("width: 36px;")
                && menu_ui.contains("font-size: 12px;"),
            "Column header menu should use compact switch controls with consistent label sizing"
        );
        assert!(
            menu_ui.contains("source: AppIcons.close;")
                && menu_ui.contains("colorize: delete-column-ta.has-hover ? #ff5c5c : #db3f3f;"),
            "Custom columns should render a red SVG delete icon"
        );
        assert!(
            slint_ui.contains("show_delete_custom_column_confirm"),
            "App window should expose confirmation state for custom-column deletion"
        );
        assert!(
            slint_ui
                .contains("root.delete_custom_playlist_column(root.delete_custom_column_index);"),
            "Confirmed deletion should invoke custom-column delete callback"
        );
        assert!(
            !slint_ui.contains(
                "root.toggle_playlist_column(index);\n            column-header-menu.close();"
            ),
            "Column toggle should not force-close the column header menu"
        );
    }

    #[test]
    fn test_playlist_header_exposes_drag_reorder_wiring() {
        let slint_ui = include_str!("../roqtune.slint");
        assert!(
            slint_ui.contains("column-header-drag-ta := TouchArea"),
            "Header should include drag TouchArea for column reordering"
        );
        assert!(
            slint_ui.contains("callback reorder_playlist_columns(int, int);"),
            "App window should expose reorder callback"
        );
    }

    #[test]
    fn test_playlist_header_uses_measured_visible_columns_band_contract() {
        let slint_ui = include_str!("../roqtune.slint");
        assert!(
            slint_ui.contains("in-out property <[int]> playlist_column_widths_px: [];"),
            "Track list should expose runtime playlist column widths"
        );
        assert!(
            slint_ui.contains("for column-header[i] in root.playlist_visible_column_headers"),
            "Header content should render from visible header model"
        );
        assert!(
            slint_ui.contains("width: root.playlist-column-width(i);"),
            "Header content should render from measured width model"
        );
        assert!(
            slint_ui
                .contains("root.playlist_columns_viewport_resized(self.columns-band-width-px);"),
            "Header width should be emitted to Rust for selection/resize hit-testing"
        );
    }

    #[test]
    fn test_layout_editor_and_splitter_callbacks_are_wired_in_slint() {
        let slint_ui = include_str!("../roqtune.slint");
        assert!(
            slint_ui.contains("callback layout_select_leaf(string);"),
            "Layout editor should expose leaf selection callback"
        );
        assert!(
            slint_ui.contains("callback layout_split_selected_leaf(int, int);"),
            "Layout editor should expose split callback with axis and panel kind"
        );
        assert!(
            slint_ui.contains("callback preview_layout_splitter_ratio(string, float);"),
            "Layout editor should expose splitter preview callback"
        );
        assert!(
            slint_ui.contains("callback commit_layout_splitter_ratio(string, float);"),
            "Layout editor should expose splitter commit callback"
        );
        assert!(
            slint_ui.contains("for splitter[i] in root.layout_splitters"),
            "Layout editor should render splitter models"
        );
    }
}
