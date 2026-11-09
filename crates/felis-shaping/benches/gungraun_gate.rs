//! Instruction-count twin of `shape_cache.rs` (`miss`) and `shape_run.rs`, run
//! under callgrind for the PR regression gate (`just bench-gate-ir`). The font
//! loads in setup, outside the measured region, and the cache or shaper each
//! bench starts from is stated by its setup.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[cfg(target_os = "linux")]
#[path = "support/cache.rs"]
mod cache;

#[cfg(target_os = "linux")]
#[path = "support/emoji.rs"]
mod emoji;

#[cfg(target_os = "linux")]
#[path = "support/run.rs"]
mod run;

#[cfg(target_os = "linux")]
#[allow(
    unused_qualifications,
    reason = "gungraun's #[library_benchmark] expansion emits `super::` paths with user spans"
)]
mod gate {
    use std::{hint::black_box, sync::Arc};

    use felis_shaping::{Font, ShapeCache, ShapedGlyph, Shaper};
    use gungraun::{LibraryBenchmarkConfig, library_benchmark, library_benchmark_group};

    use crate::{cache, emoji, run};

    const FONT_DIR: &str = "FELIS_TEST_FONT_DIR";

    /// The pinned face only: the host default would make the base and head
    /// counts depend on whichever font the runner happens to resolve.
    fn font() -> Font {
        assert!(
            std::env::var_os(FONT_DIR).is_some(),
            "{FONT_DIR} is unset: run the gate in the dev shell, which sets it"
        );
        Font::load_test_font_or_default().expect("load the pinned test font")
    }

    pub(crate) struct CacheInput {
        font: Font,
        cache: ShapeCache,
    }

    fn setup_cold_cache() -> CacheInput {
        CacheInput {
            font: font(),
            cache: ShapeCache::default(),
        }
    }

    // Every glyph misses: the cache and its rasterizer start empty.
    #[library_benchmark(setup = setup_cold_cache)]
    #[bench::miss()]
    fn shape_cache(mut input: CacheInput) -> CacheInput {
        for &ch in cache::GLYPHS {
            black_box(input.cache.get_or_insert(&input.font, ch, cache::PX));
        }
        input
    }

    pub(crate) struct EmojiInput {
        font: Arc<Font>,
        glyphs: &'static [char],
        cache: ShapeCache,
    }

    fn setup_emoji(source: &str) -> EmojiInput {
        let (font, glyphs) = match source {
            "cbdt" => (emoji::cbdt(), emoji::CBDT),
            "colr" => (emoji::colr(), emoji::COLR),
            _ => (emoji::colr_v1(), emoji::COLR_V1),
        };
        EmojiInput {
            font,
            glyphs,
            cache: ShapeCache::default(),
        }
    }

    // Every emoji misses, as in `shape_cache`.
    #[library_benchmark(setup = setup_emoji)]
    #[bench::miss_emoji_cbdt("cbdt")]
    #[bench::miss_emoji_colr("colr")]
    #[bench::miss_emoji_colrv1("colrv1")]
    fn shape_cache_emoji(mut input: EmojiInput) -> EmojiInput {
        for &ch in input.glyphs {
            black_box(input.cache.get_or_insert(&input.font, ch, emoji::PX));
        }
        input
    }

    pub(crate) struct RunInput {
        font: Font,
        features: Vec<String>,
        shaper: Option<Shaper>,
    }

    fn setup_run(warm: bool) -> RunInput {
        let font = font();
        let features = run::features();
        let shaper = warm.then(|| {
            let mut shaper = Shaper::new();
            drop(shaper.shape_run(&font, run::PX, &features, run::ROW));
            shaper
        });
        RunInput {
            font,
            features,
            shaper,
        }
    }

    // `reused_shaper` starts from a shaper that has already shaped the row
    // once; `fresh_shaper` builds its shaper inside the measured region, as
    // the Criterion twin does.
    #[library_benchmark(setup = setup_run)]
    #[bench::reused_shaper(true)]
    #[bench::fresh_shaper(false)]
    fn shape_run(mut input: RunInput) -> (RunInput, Vec<ShapedGlyph>) {
        let mut shaper = input.shaper.take().unwrap_or_default();
        let glyphs = black_box(shaper.shape_run(&input.font, run::PX, &input.features, run::ROW));
        input.shaper = Some(shaper);
        (input, glyphs)
    }

    library_benchmark_group!(
        name = shaping,
        benchmarks = [shape_cache, shape_cache_emoji, shape_run]
    );

    gungraun::main!(
        config = LibraryBenchmarkConfig::default().pass_through_env(FONT_DIR);
        library_benchmark_groups = shaping
    );

    pub(super) fn run() {
        main();
    }
}

#[cfg(target_os = "linux")]
fn main() {
    gate::run();
}

#[cfg(not(target_os = "linux"))]
#[expect(
    clippy::print_stderr,
    reason = "the notice is the bench's only output where Valgrind is unavailable"
)]
fn main() {
    eprintln!("gungraun_gate: callgrind runs on Linux only; nothing to measure here");
}
