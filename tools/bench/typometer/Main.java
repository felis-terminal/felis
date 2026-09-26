/*
 * Headless entry point for Typometer (github.com/pavelfatin/typometer,
 * Apache-2.0), whose own `main` opens a Swing window and offers no other
 * way in — a benchmark that cannot start without someone clicking Start
 * cannot join a suite that runs unattended for an hour.
 *
 * Only the entry point is ours. The measurement is upstream's, driven
 * through its public `Benchmark` API: press a key with java.awt.Robot,
 * then poll the screen until the pixel the glyph lands on changes
 * color. Re-implementing that would cost the one thing that makes the
 * tool worth building — every Typometer figure in circulation was taken
 * this way, and a rewrite of the timing loop would not be comparable
 * with any of them.
 *
 * Output is one JSON object carrying the per-character samples in the
 * order they were typed. Summarizing here would throw the distribution
 * away, and with a ~8 ms spread around a ~24 ms mean the distribution is
 * most of what an input-latency number means (suites.py:suite_latency).
 */
package felis.bench;

import com.pavelfatin.typometer.benchmark.Benchmark;
import com.pavelfatin.typometer.benchmark.BenchmarkListener;
import com.pavelfatin.typometer.benchmark.Parameters;
import com.pavelfatin.typometer.screen.ScreenAccessor;
import com.pavelfatin.typometer.statistics.Statistics;
import com.pavelfatin.typometer.statistics.SummaryStatistics;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.StringJoiner;

public final class Main {
    // Upstream's own defaults (Application.DEFAULT_PARAMETERS), so a run
    // from here and a run through the UI measure the same thing.
    private static final int COUNT = 200;
    private static final int DELAY = 150;
    private static final int PAUSE_PERIOD = 50;
    private static final int PAUSE_LENGTH = 1000;

    private Main() {
    }

    public static void main(String[] args) {
        int count = COUNT;
        int delay = DELAY;
        Path json = null;

        for (int i = 0; i < args.length; i++) {
            String arg = args[i];
            if (i + 1 == args.length) {
                die("missing value for " + arg);
            }
            String value = args[++i];
            switch (arg) {
                case "--count":
                    count = Integer.parseInt(value);
                    break;
                case "--delay":
                    delay = Integer.parseInt(value);
                    break;
                case "--json":
                    json = Paths.get(value);
                    break;
                default:
                    die("unknown argument: " + arg);
            }
        }

        List<Double> samples = new ArrayList<>();
        String[] failure = {null};

        BenchmarkListener listener = new BenchmarkListener() {
            @Override
            public void onStart(Parameters parameters) {
            }

            @Override
            public void onPhase(String title) {
                // "Character 7 / 200..." arrives once per keystroke; the
                // phases worth a line are the setup ones around them.
                if (!title.startsWith("Character ")) {
                    System.err.println("  typometer: " + title);
                }
            }

            @Override
            public void onResult(double value) {
                samples.add(value);
            }

            @Override
            public void onError(String message) {
                failure[0] = message;
            }

            @Override
            public void onFinish() {
            }
        };

        ScreenAccessor accessor = ScreenAccessor.create(ScreenAccessor.isNativeApiSupported());
        Benchmark benchmark = Benchmark.create();
        try {
            // Synchronous typing, upstream's default: each keystroke
            // waits for its own glyph, so one sample is one keypress
            // answered and never a backlog draining.
            benchmark.run(
                    new Parameters(count, delay, false, PAUSE_PERIOD, PAUSE_LENGTH),
                    false, accessor, listener);
        } finally {
            benchmark.dispose();
            accessor.dispose();
        }

        if (failure[0] != null) {
            die(failure[0]);
        }
        if (samples.isEmpty()) {
            die("finished without a single sample");
        }

        Statistics stats = SummaryStatistics.analyze(samples);
        System.err.printf(
                Locale.ROOT,
                "  typometer: %d samples, min %.1f mean %.1f max %.1f SD %.1f ms%n",
                stats.getCount(), stats.getMin(), stats.getMean(),
                stats.getMax(), stats.getStandardDeviation());

        String payload = toJson(count, delay, samples);
        if (json == null) {
            System.out.print(payload);
        } else {
            write(json, payload);
        }
        // AWT keeps non-daemon threads alive once Robot has touched it.
        System.exit(0);
    }

    private static String toJson(int count, int delay, List<Double> samples) {
        StringJoiner values = new StringJoiner(", ");
        for (double sample : samples) {
            values.add(String.format(Locale.ROOT, "%.3f", sample));
        }
        return "{\n"
                + "  \"count\": " + count + ",\n"
                + "  \"delay_ms\": " + delay + ",\n"
                + "  \"samples_ms\": [" + values + "]\n"
                + "}\n";
    }

    private static void write(Path path, String text) {
        try {
            Files.write(path, text.getBytes(StandardCharsets.UTF_8));
        } catch (IOException e) {
            die("cannot write " + path + ": " + e.getMessage());
        }
    }

    private static void die(String message) {
        System.err.println("typometer: " + message);
        System.exit(1);
    }
}
