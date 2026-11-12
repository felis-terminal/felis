use super::*;

#[test]
fn window_attributes_flag_transparent_surface_when_requested() {
    // Without it the OS composites the window opaque and the renderer's
    // bg alpha is silently ignored.
    let attrs = window_attributes("felis".to_owned(), true, true, None);
    assert!(attrs.transparent);
}

#[test]
fn window_attributes_keep_rounded_corners_on_macos_when_chromeless() {
    // winit gives OS rounded corners and shadow only to a `Titled`
    // window and selects `Borderless` whenever `decorations` is false,
    // so on macOS the flag stays `true` and the titlebar is emptied
    // instead.
    let attrs = window_attributes("felis".to_owned(), false, false, None);
    if cfg!(target_os = "macos") {
        assert!(attrs.decorations);
    } else {
        assert!(!attrs.decorations);
    }
}

#[test]
fn the_configured_baseline_routes_font_size_through_the_sanitizer() {
    // Pins the integration: the baseline goes through
    // `sanitized_font_size_logical_px`, not `cfg.font.size_px` raw.
    let oversize = config::EffectiveConfig {
        font: config::FontConfig {
            size_px: Some(200.0),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        configured_font_size_logical_px(&oversize),
        Some(MAX_FONT_SIZE_LOGICAL_PX),
        "200 px config size must clamp to MAX",
    );

    let nan = config::EffectiveConfig {
        font: config::FontConfig {
            size_px: Some(f32::NAN),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(
        configured_font_size_logical_px(&nan).is_none(),
        "NaN must drop to None",
    );
}

#[test]
fn renderer_config_from_leaves_the_font_size_to_the_window() {
    // The renderer takes a physical size, and the scale factor only
    // exists once the window does; filling it from the logical value
    // rasterized glyphs at half size on Retina.
    let cfg = config::EffectiveConfig {
        font: config::FontConfig {
            size_px: Some(15.0),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(renderer_config_from(&cfg).font_size_physical_px.is_none());
}

#[test]
fn renderer_config_from_default_config_is_all_none() {
    let rc = renderer_config_from(&config::EffectiveConfig::default());
    assert!(rc.font_family.is_none());
    assert!(rc.font_size_physical_px.is_none());
    assert_eq!(rc.font_fallbacks, Vec::<FaceSpec>::new());
    assert!(rc.theme.fg.is_none());
    assert!(rc.theme.bg.is_none());
    assert!(rc.theme.cursor.is_none());
    assert!(rc.theme.palette.is_empty());
}

mod post_chain {
    use super::*;

    fn resolved(text: &str) -> config::EffectiveConfig {
        config::ConfigDocument::parse(text, None)
            .unwrap()
            .resolve(GUI_CLIENT_ID)
            .0
    }

    #[test]
    fn an_unreadable_file_stage_is_dropped_together_with_its_animation() {
        let dir = std::env::temp_dir().join(format!("felis-post-chain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let readable = dir.join("glow.wgsl");
        std::fs::write(&readable, "// glow").unwrap();
        let missing = dir.join("absent.wgsl");
        let cfg = resolved(&format!(
            "[shader]\npost = [\
             {{ builtin = \"trail\", animation = {{ on = [\"bell\"], duration_ms = 100 }} }},\
             {{ file = {missing:?}, animation = {{ on = [\"bell\"], duration_ms = 200 }} }},\
             {{ file = {readable:?}, animation = {{ on = [\"bell\"], duration_ms = 300 }} }},\
             {{ builtin = \"trail\" }}]\n"
        ));

        let chain = resolve_post_chain(&cfg);
        std::fs::remove_dir_all(&dir).unwrap();

        let labels: Vec<&str> = chain.sources.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "builtin trail",
                readable.display().to_string().as_str(),
                "builtin trail"
            ]
        );
        assert_eq!(chain.sources[1].wgsl, "// glow");
        let durations: Vec<Option<u32>> = chain
            .animations
            .iter()
            .map(|a| a.as_ref().map(|a| a.duration_ms))
            .collect();
        assert_eq!(durations, [Some(100), Some(300), None]);
    }

    #[test]
    fn stage_params_map_to_renderer_params_by_name() {
        let cfg = resolved(
            "[shader]\npost = { builtin = \"trail\", params = { on = true, steps = 4, strength = 0.5 } }\n",
        );
        let chain = resolve_post_chain(&cfg);
        assert_eq!(
            chain.sources[0].params,
            [
                ("on".to_owned(), PostParam::Bool(true)),
                ("steps".to_owned(), PostParam::Int(4)),
                ("strength".to_owned(), PostParam::Float(0.5)),
            ]
        );
    }
}

#[test]
#[allow(clippy::float_cmp)]
fn css_shorthand_theme_color_falls_back_to_the_defaults_end_to_end() {
    // Pins end to end (config -> RendererConfig -> Theme): `#abc` yields
    // the built-in color, and the rejection must survive every hop or
    // the painted color would not match the reported theme.
    let short = config::EffectiveConfig {
        theme: config::ThemeConfig {
            foreground: Some("#abc".into()),
            background: Some("#def".into()),
            ..Default::default()
        },
        cursor: config::CursorConfig {
            color: Some("#fff".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let theme = Theme::from_config(&renderer_config_from(&short).theme);
    let default = Theme::default();
    assert_eq!(theme.fg, default.fg, "shorthand fg keeps the default");
    assert_eq!(theme.bg, default.bg, "shorthand bg keeps the default");
    assert_eq!(
        theme.cursor, None,
        "shorthand cursor color falls back to reverse video",
    );
}

#[test]
fn renderer_config_from_full_config_carries_every_field_through() {
    let cfg = config::EffectiveConfig {
        font: config::FontConfig {
            family: Some("JetBrains Mono".to_owned()),
            size_px: Some(15.0),
            cell_width_scale: Some(1.1),
            cell_height_scale: Some(1.25),
            weight: Some(500),
            axes: std::collections::BTreeMap::from([("wdth".to_owned(), 90.0)]),
            fallback: vec![
                config::FontStyleConfig {
                    family: Some("Noto Sans CJK JP".to_owned()),
                    ..Default::default()
                },
                config::FontStyleConfig {
                    family: Some("Noto Color Emoji".to_owned()),
                    features: Some(Vec::new()),
                    ..Default::default()
                },
            ],
            features: Vec::new(),
            bold: config::FontStyleConfig {
                family: Some("Iosevka Bold".to_owned()),
                features: Some(vec!["ss01".to_owned()]),
                weight: Some(800),
                axes: std::collections::BTreeMap::from([("GRAD".to_owned(), 50.0)]),
            },
            italic: config::FontStyleConfig {
                family: Some("Victor Mono".to_owned()),
                ..Default::default()
            },
            bold_italic: config::FontStyleConfig::default(),
        },
        cursor: config::CursorConfig {
            color: Some("#ffaa00".to_owned()),
            ..Default::default()
        },
        theme: config::ThemeConfig {
            foreground: Some("#cdcdcd".to_owned()),
            background: Some("#101010".to_owned()),
            palette: config::PaletteConfig {
                red: Some("#ff5555".to_owned()),
                bright_blue: Some("#5555ff".to_owned()),
                indexed: std::collections::BTreeMap::from([(
                    "16".to_owned(),
                    "#d08770".to_owned(),
                )]),
                ..Default::default()
            },
            minimum_contrast: 4.5,
        },
        ..Default::default()
    };
    let rc = renderer_config_from(&cfg);
    assert_eq!(rc.font_family.as_deref(), Some("JetBrains Mono"));
    assert_eq!(rc.cell_scale, Some([1.1, 1.25]));
    // Inherit (no features) vs opt-out (empty list) must survive the mapping.
    assert_eq!(
        rc.font_fallbacks,
        vec![
            FaceSpec::named("Noto Sans CJK JP"),
            FaceSpec {
                family: Some("Noto Color Emoji".to_owned()),
                features: Some(Vec::new()),
                ..FaceSpec::default()
            },
        ],
    );
    assert_eq!(rc.font_bold.family.as_deref(), Some("Iosevka Bold"));
    assert_eq!(rc.font_bold.features, Some(vec!["ss01".to_owned()]));
    assert_eq!(rc.font_weight, Some(500));
    assert_eq!(
        rc.font_axes,
        [AxisSetting {
            tag: *b"wdth",
            value: 90.0
        }]
    );
    assert_eq!(rc.font_bold.weight, Some(800));
    assert_eq!(
        rc.font_bold.axes,
        [AxisSetting {
            tag: *b"GRAD",
            value: 50.0
        }]
    );
    assert_eq!(rc.font_italic.weight, None);
    assert_eq!(rc.font_italic.family.as_deref(), Some("Victor Mono"));
    assert_eq!(rc.font_italic.features, None);
    assert_eq!(rc.font_bold_italic.family, None);
    assert_eq!(rc.theme.fg.as_deref(), Some("#cdcdcd"));
    assert_eq!(rc.theme.bg.as_deref(), Some("#101010"));
    assert_eq!(rc.theme.cursor.as_deref(), Some("#ffaa00"));
    assert_eq!(rc.theme.minimum_contrast, Some(4.5));
    assert_eq!(
        renderer_config_from(&config::EffectiveConfig::default())
            .theme
            .minimum_contrast,
        None,
        "the default ratio of 1 maps to no adjustment"
    );
    // `PaletteConfig::into_overrides` lays out by SGR index: slot 1 is
    // red, 12 is bright_blue, and `indexed` carries cube slot 16.
    assert_eq!(
        rc.theme.palette.get(&1).map(String::as_str),
        Some("#ff5555")
    );
    assert_eq!(
        rc.theme.palette.get(&12).map(String::as_str),
        Some("#5555ff")
    );
    assert_eq!(
        rc.theme.palette.get(&16).map(String::as_str),
        Some("#d08770")
    );
    // Untouched slots are absent; the renderer keeps the xterm baseline.
    assert!(!rc.theme.palette.contains_key(&0), "slot 0 stays at xterm");
    assert!(
        !rc.theme.palette.contains_key(&15),
        "slot 15 stays at xterm"
    );
}

mod font_differs {
    use super::*;

    #[test]
    fn a_reload_after_zoom_resizes_the_font_even_with_the_size_unchanged() {
        assert!(
            live_font_size_differs(18.0, 14.0),
            "zoomed in, size_px unchanged"
        );
        assert!(
            live_font_size_differs(14.0, 16.0),
            "size_px edited, no zoom"
        );
        assert!(!live_font_size_differs(14.0, 14.0), "neither");
    }

    #[test]
    fn a_size_only_edit_takes_the_size_only_reload_path() {
        // `App::reload_config` picks its renderer call from these
        // predicates; a size-only edit must land on `reload_font_size`,
        // since the full `reload_font` rediscovers the faces. The size is not
        // on `RendererConfig`, so the invariant spans both inputs the
        // reload reads.
        let base = config::EffectiveConfig {
            font: config::FontConfig {
                family: Some("JetBrains Mono".to_owned()),
                size_px: Some(14.0),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bigger = base.clone();
        bigger.font.size_px = Some(16.0);

        assert_ne!(
            configured_font_size_logical_px(&bigger),
            configured_font_size_logical_px(&base),
            "the reload's `size_changed` arm must see the edit",
        );
        let (base_rc, bigger_rc) = (renderer_config_from(&base), renderer_config_from(&bigger));
        assert!(
            !font_stack_inputs_differ(&base_rc, &bigger_rc),
            "size-only edit must take the fast path",
        );
        assert!(
            !font_features_differ(&base_rc, &bigger_rc),
            "size-only edit must not invalidate for features",
        );
        assert!(
            !font_settings_differ(&base_rc, &bigger_rc),
            "the size no longer rides RendererConfig, so nothing there changed",
        );
    }

    #[test]
    fn font_stack_inputs_differ_isolates_stack_edits() {
        // Pins the split between the slow path (`reload_font` rediscovers
        // the faces) and the fast paths that keep the `FontStack`.
        let base = RendererConfig {
            font_family: Some("JetBrains Mono".to_owned()),
            font_fallbacks: vec![FaceSpec::named("Noto Sans CJK JP")],
            ..Default::default()
        };
        let mut family = base.clone();
        family.font_family = Some("Hack".to_owned());
        assert!(font_settings_differ(&base, &family));
        assert!(font_stack_inputs_differ(&base, &family));
        let mut fallback = base.clone();
        fallback.font_fallbacks = vec![FaceSpec::named("Noto Color Emoji")];
        assert!(font_settings_differ(&base, &fallback));
        assert!(font_stack_inputs_differ(&base, &fallback));
        let mut features = base.clone();
        features.font_features = vec!["calt".to_owned()];
        assert!(font_settings_differ(&base, &features));
        assert!(
            !font_stack_inputs_differ(&base, &features),
            "features-only edit must take the shape-time fast path",
        );
        // A new styled face needs the rediscovery like a base-family change.
        let mut bold = base.clone();
        bold.font_bold = FaceSpec::named("Iosevka");
        assert!(font_settings_differ(&base, &bold));
        assert!(font_stack_inputs_differ(&base, &bold));
        let mut cell = base.clone();
        cell.cell_scale = Some([1.0, 1.2]);
        assert!(font_settings_differ(&base, &cell));
        assert!(
            !font_stack_inputs_differ(&base, &cell),
            "a cell-scale edit keeps the faces",
        );
        let mut weight = base.clone();
        weight.font_weight = Some(500);
        assert!(font_stack_inputs_differ(&base, &weight));
        let mut axes = base.clone();
        axes.font_axes = vec![AxisSetting {
            tag: *b"wdth",
            value: 90.0,
        }];
        assert!(font_stack_inputs_differ(&base, &axes));
        let mut bold_weight = base.clone();
        bold_weight.font_bold.weight = Some(800);
        assert!(font_settings_differ(&base, &bold_weight));
        assert!(font_stack_inputs_differ(&base, &bold_weight));
        assert!(!font_settings_differ(&base, &base));
        assert!(!font_stack_inputs_differ(&base, &base));
    }

    #[test]
    fn font_settings_differ_ignores_theme_changes() {
        // A pure color change must not take the atlas-rebuild slow path.
        let base = RendererConfig {
            font_family: Some("JetBrains Mono".to_owned()),
            theme: ThemeConfig {
                fg: Some("#ffffff".to_owned()),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut other = base.clone();
        other.theme.fg = Some("#000000".to_owned());
        other.theme.bg = Some("#101010".to_owned());
        other.theme.cursor = Some("#ff00ff".to_owned());
        assert!(!font_settings_differ(&base, &other));
    }
}

#[test]
fn reported_theme_differs_tracks_only_the_reported_colors() {
    // The reload path re-sends `ConfigureTheme` off this predicate, and
    // the frame carries fg / bg / cursor and the palette: a stale one
    // leaves OSC 4 / 10 / 11 / 12 answering the attach-time color; an
    // opacity- or contrast-only edit would resend identical colors.
    let base = RendererConfig {
        theme: ThemeConfig {
            fg: Some("#ffffff".to_owned()),
            bg: Some("#000000".to_owned()),
            cursor: Some("#ff00ff".to_owned()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(!reported_theme_differs(&base, &base));

    let mut fg = base.clone();
    fg.theme.fg = Some("#eeeeee".to_owned());
    assert!(reported_theme_differs(&base, &fg));

    let mut bg = base.clone();
    bg.theme.bg = Some("#101010".to_owned());
    assert!(reported_theme_differs(&base, &bg));

    let mut cursor = base.clone();
    cursor.theme.cursor = Some("#00ff00".to_owned());
    assert!(reported_theme_differs(&base, &cursor));

    let mut palette_only = base.clone();
    palette_only.theme.palette.insert(1, "#ff0000".to_owned());
    assert!(reported_theme_differs(&base, &palette_only));

    let mut opacity_only = base.clone();
    opacity_only.theme.background_opacity = Some(0.8);
    assert!(!reported_theme_differs(&base, &opacity_only));

    let mut contrast_only = base.clone();
    contrast_only.theme.minimum_contrast = Some(4.5);
    assert!(!reported_theme_differs(&base, &contrast_only));
}
