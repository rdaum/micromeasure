// Sample statistics.
package micromeasure

import "core:math"
import "core:slice"

// Computes robust statistics over per-operation samples.
compute_stats :: proc(samples: []f64) -> Stats {
	if len(samples) == 0 {
		return Stats{}
	}

	sorted := make([]f64, len(samples))
	defer delete(sorted)
	copy(sorted, samples)
	slice.sort_by(sorted, proc(a, b: f64) -> bool {
		return a < b
	})

	stats := Stats {
		median = percentile_sorted(sorted, 0.50),
		p95    = percentile_sorted(sorted, 0.95),
		min    = sorted[0],
		max    = sorted[len(sorted) - 1],
	}

	sum := f64(0)
	for sample in samples {
		sum += sample
	}
	stats.mean = sum / f64(len(samples))

	variance := f64(0)
	for sample in samples {
		difference := sample - stats.mean
		variance += difference * difference
	}
	if len(samples) > 1 {
		variance /= f64(len(samples) - 1)
	}
	stats.stddev = math.sqrt(variance)
	if stats.mean > 0 {
		stats.cv = stats.stddev / stats.mean
	}

	deviations := make([]f64, len(samples))
	defer delete(deviations)
	for sample, i in samples {
		deviations[i] = math.abs(sample - stats.median)
	}
	slice.sort_by(deviations, proc(a, b: f64) -> bool {
		return a < b
	})
	stats.mad = percentile_sorted(deviations, 0.50)

	// With zero MAD, every value different from the median is an outlier.
	threshold := 3 * stats.mad
	for sample in samples {
		if math.abs(sample - stats.median) > threshold {
			stats.outliers += 1
		}
	}
	return stats
}

// Returns the coefficient of variation for a sample set.
coefficient_of_variation :: proc(samples: []f64) -> f64 {
	if len(samples) < 2 {
		return 1
	}
	sum := f64(0)
	for sample in samples {
		sum += sample
	}
	mean := sum / f64(len(samples))
	if mean == 0 {
		return 1
	}
	variance := f64(0)
	for sample in samples {
		difference := sample - mean
		variance += difference * difference
	}
	variance /= f64(len(samples) - 1)
	return math.sqrt(variance) / mean
}

// Returns the nearest-rank percentile of a sorted sample set.
percentile_sorted :: proc(sorted: []f64, fraction: f64) -> f64 {
	if len(sorted) == 0 {
		return 0
	}
	rank := int(math.ceil(fraction * f64(len(sorted))))
	index := rank - 1
	if index < 0 {
		index = 0
	}
	if index >= len(sorted) {
		index = len(sorted) - 1
	}
	return sorted[index]
}
